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
use shared_types::{
    AGENT_FILE_SERVER_PORT, DBX_PORT, GRPC_DEFAULT_PORT, HTTP_DEFAULT_PORT, NOVNC_PORT,
    ServiceType, WS_TERMINAL_PORT,
};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use tracing::{debug, info, warn};

#[cfg(feature = "kubernetes")]
use super::k8s_pod::K8sPodOps;
#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::KubernetesRuntime;

/// Agent Runner HTTP 端口（使用 shared_types 共享常量）
const AGENT_HTTP_PORT: u32 = HTTP_DEFAULT_PORT as u32;

/// Agent Runner gRPC 端口（使用 shared_types 共享常量）
const AGENT_GRPC_PORT: u32 = GRPC_DEFAULT_PORT as u32;

/// Agent Runner noVNC 端口（使用 shared_types 共享常量）
const AGENT_NOVNC_PORT: u32 = NOVNC_PORT as u32;

/// Agent Runner WS 终端中间层端口（agent_runner tokio-tungstenite 监听；Pingora TtydProxy 路由到此）
const AGENT_WS_TERMINAL_PORT: u32 = WS_TERMINAL_PORT as u32;

/// DBX 数据库 Web GUI 端口（agent-runner 镜像 supervisor 恒起；Pingora /api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id} 路由到此）
const AGENT_DBX_PORT: u32 = DBX_PORT as u32;

/// K8s 标准标签前缀
const LABEL_PREFIX: &str = "app.kubernetes.io";

/// RCoder 自定义标签前缀
const RCODER_LABEL_PREFIX: &str = "rcoder.io";

/// 构建 K8s 标准标签
///
/// 根据 Kubernetes 推荐标签规范，为资源添加标准标签：
/// - `app.kubernetes.io/name`: 应用名称（根据 ServiceType 动态生成）
/// - `app.kubernetes.io/instance`: 实例标识（project_id 或 user_id）
/// - `app.kubernetes.io/version`: 版本
/// - `app.kubernetes.io/component`: 组件
/// - `app.kubernetes.io/managed-by`: 管理者
/// - `app.kubernetes.io/part-of`: 所属系统
/// - `rcoder.io/service-type`: 服务类型
/// - `rcoder.io/identifier`: 业务标识
pub(crate) fn build_standard_labels(
    identifier: &str,
    service_type: &ServiceType,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();

    // K8s 推荐标签
    // app.kubernetes.io/name: 应用名称，使用 service_type 的字符串表示
    labels.insert(format!("{}/name", LABEL_PREFIX), service_type.to_string());
    labels.insert(format!("{}/instance", LABEL_PREFIX), identifier.to_string());
    labels.insert(format!("{}/version", LABEL_PREFIX), "v1".to_string());
    labels.insert(format!("{}/component", LABEL_PREFIX), "agent".to_string());
    labels.insert(
        format!("{}/managed-by", LABEL_PREFIX),
        "rcoder-runtime".to_string(),
    );
    labels.insert(format!("{}/part-of", LABEL_PREFIX), "rcoder".to_string());

    // RCoder 自定义标签
    labels.insert(
        format!("{}/service-type", RCODER_LABEL_PREFIX),
        service_type.to_string(),
    );
    labels.insert(
        format!("{}/identifier", RCODER_LABEL_PREFIX),
        identifier.to_string(),
    );

    labels
}

/// 构建 K8s Selector 标签
///
/// Selector 只包含必要的标签，用于精确匹配 Pod
fn build_selector_labels(identifier: &str, service_type: &ServiceType) -> BTreeMap<String, String> {
    let mut selector = BTreeMap::new();

    // 使用标准标签进行选择
    selector.insert(format!("{}/name", LABEL_PREFIX), service_type.to_string());
    selector.insert(format!("{}/instance", LABEL_PREFIX), identifier.to_string());
    selector.insert(
        format!("{}/managed-by", LABEL_PREFIX),
        "rcoder-runtime".to_string(),
    );

    // 使用自定义标签进行精确匹配
    selector.insert(
        format!("{}/identifier", RCODER_LABEL_PREFIX),
        identifier.to_string(),
    );

    selector
}

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
    /// Service 暴露以下端口：
    /// - HTTP 8086：健康检查、状态查询
    /// - gRPC 50051：rcoder 与 agent-runner 通信
    /// - noVNC 6080：Web VNC 访问
    /// - ttyd 7681：Web 终端访问
    ///
    /// selector 使用与 Pod 相同的 labels（`app.kubernetes.io/managed-by=rcoder-runtime` + identifier label）。
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
        let selector = build_selector_labels(identifier, service_type);

        let service = Service {
            metadata: ObjectMeta {
                name: Some(svc_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(build_standard_labels(identifier, service_type)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                selector: Some(selector),
                ports: Some(vec![
                    ServicePort {
                        name: Some("http".to_string()),
                        port: AGENT_HTTP_PORT as i32,
                        target_port: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                AGENT_HTTP_PORT as i32,
                            ),
                        ),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("grpc".to_string()),
                        port: AGENT_GRPC_PORT as i32,
                        target_port: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                AGENT_GRPC_PORT as i32,
                            ),
                        ),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("novnc".to_string()),
                        port: AGENT_NOVNC_PORT as i32,
                        target_port: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                AGENT_NOVNC_PORT as i32,
                            ),
                        ),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("ws-terminal".to_string()),
                        port: AGENT_WS_TERMINAL_PORT as i32,
                        target_port: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                AGENT_WS_TERMINAL_PORT as i32,
                            ),
                        ),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("file-server".to_string()),
                        port: AGENT_FILE_SERVER_PORT as i32,
                        target_port: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                AGENT_FILE_SERVER_PORT as i32,
                            ),
                        ),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("dbx".to_string()),
                        port: AGENT_DBX_PORT as i32,
                        target_port: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                AGENT_DBX_PORT as i32,
                            ),
                        ),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                ]),
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
