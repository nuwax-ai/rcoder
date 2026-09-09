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
use kube::api::{Api, DeleteParams, ObjectMeta, Patch, PostParams};
#[cfg(feature = "kubernetes")]
use shared_types::{
    AGENT_FILE_SERVER_PORT, APP_CLI_ADMIN_PORT, DBX_PORT, GRPC_DEFAULT_PORT, HTTP_DEFAULT_PORT,
    NOVNC_PORT, ServiceType, WS_TERMINAL_PORT,
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

/// agent per-pod Service 的 TCP 端口条目
fn tcp_service_port(name: &str, port: u16) -> ServicePort {
    ServicePort {
        name: Some(name.to_string()),
        port: port as i32,
        target_port: Some(
            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(port as i32),
        ),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    }
}

/// agent per-pod Service 端口清单（创建与存量收敛共用同一期望）。
///
/// 基础六端口恒暴露；UserappBuilder 追加 app-cli 管理/日志端口——
/// app_manager `log_api_base` dev 分支按 `{svc}:{APP_CLI_ADMIN_PORT}` 连容器内
/// app-cli，ClusterIP 未声明的端口无 kube-proxy/Cilium 转发规则（SYN 被丢弃
/// → 连接超时）。其他 agent 类型容器内不跑 app-cli，不暴露。
fn agent_service_ports(service_type: &ServiceType) -> Vec<ServicePort> {
    let mut ports = vec![
        tcp_service_port("http", AGENT_HTTP_PORT as u16),
        tcp_service_port("grpc", AGENT_GRPC_PORT as u16),
        tcp_service_port("novnc", AGENT_NOVNC_PORT as u16),
        tcp_service_port("ws-terminal", AGENT_WS_TERMINAL_PORT as u16),
        tcp_service_port("file-server", AGENT_FILE_SERVER_PORT),
        tcp_service_port("dbx", AGENT_DBX_PORT as u16),
    ];
    if matches!(service_type, ServiceType::UserappBuilder) {
        ports.push(tcp_service_port("app-cli-admin", APP_CLI_ADMIN_PORT));
    }
    ports
}

/// 组装 agent per-pod Service 的期望全量 spec（缺失创建与存量 SSA 收敛同源）
fn agent_service_object(
    namespace: &str,
    svc_name: &str,
    identifier: &str,
    service_type: &ServiceType,
) -> Service {
    Service {
        metadata: ObjectMeta {
            name: Some(svc_name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(build_standard_labels(identifier, service_type)),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(build_selector_labels(identifier, service_type)),
            ports: Some(agent_service_ports(service_type)),
            ..Default::default()
        }),
        status: None,
    }
}

/// Service 是否已声明某端口（端口值口径；ClusterIP 只路由已声明的端口）
fn service_exposes_port(svc: &Service, port: u16) -> bool {
    svc.spec
        .as_ref()
        .and_then(|spec| spec.ports.as_ref())
        .is_some_and(|ports| ports.iter().any(|p| p.port == port as i32))
}

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
    /// Service 暴露以下端口（`agent_service_ports` 单一事实源）：
    /// - HTTP 8086：健康检查、状态查询（Pingora 统一入口）
    /// - gRPC 50051：rcoder 与 agent-runner 通信
    /// - noVNC 6080 / ws-terminal 17681
    /// - file-server 60000 / dbx 4224
    /// - app-cli-admin 3010：仅 UserappBuilder（app-cli 管理/日志 API）
    ///
    /// selector 使用与 Pod 相同的 labels（`app.kubernetes.io/managed-by=rcoder-runtime` + identifier label）。
    /// 已存在则跳过——例外：builder Service 缺 app-cli-admin 端口时 SSA patch
    /// 定向收敛（见函数内注释）。
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
            Ok(existing) => {
                // 定向收敛：早期版本创建的 builder Service 缺 app-cli-admin
                // （APP_CLI_ADMIN_PORT）端口，dev 日志链路按 `{svc}:3010` 连接
                // 必超时——SSA patch 写入全量期望 spec 补齐（存量端口随
                // reconcile 收敛，范式对齐 apply_app_service）。仅 UserappBuilder
                // 触发：读路径自愈（get_container_info 每次经过这里）不对其他
                // agent 类型写放大。
                if matches!(service_type, ServiceType::UserappBuilder)
                    && !service_exposes_port(&existing, APP_CLI_ADMIN_PORT)
                {
                    let desired =
                        agent_service_object(&self.namespace, &svc_name, identifier, service_type);
                    let body = serde_json::to_value(&desired).map_err(|e| {
                        ContainerRuntimeError::K8sError(format!("serialize service: {e}"))
                    })?;
                    services
                        .patch(&svc_name, &Self::ssa_patch_params(), &Patch::Apply(body))
                        .await
                        .map_err(|e| {
                            ContainerRuntimeError::K8sError(format!(
                                "patch agent service '{svc_name}': {e}"
                            ))
                        })?;
                    info!(
                        "[K8S] Service {} patched to expose app-cli-admin {}",
                        svc_name, APP_CLI_ADMIN_PORT
                    );
                    return Ok(());
                }
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

        let service = agent_service_object(&self.namespace, &svc_name, identifier, service_type);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_service_ports_expose_app_cli_admin() {
        let ports = agent_service_ports(&ServiceType::UserappBuilder);
        let admin = ports
            .iter()
            .find(|p| p.name.as_deref() == Some("app-cli-admin"))
            .expect("builder Service 必须暴露 app-cli-admin");
        assert_eq!(admin.port, APP_CLI_ADMIN_PORT as i32);
        assert_eq!(
            admin.target_port,
            Some(
                k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                    APP_CLI_ADMIN_PORT as i32
                )
            )
        );
        // 端口名必须唯一（K8s 校验 "port names must be unique"）
        let mut names: Vec<_> = ports.iter().filter_map(|p| p.name.clone()).collect();
        names.sort();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total, "端口名重复: {names:?}");
        // 基础六端口仍在
        for expected in [
            AGENT_HTTP_PORT,
            AGENT_GRPC_PORT,
            AGENT_NOVNC_PORT,
            AGENT_WS_TERMINAL_PORT,
            AGENT_FILE_SERVER_PORT as u32,
            AGENT_DBX_PORT,
        ] {
            assert!(
                ports.iter().any(|p| p.port == expected as i32),
                "基础端口 {expected} 缺失"
            );
        }
    }

    #[test]
    fn other_agent_types_do_not_expose_app_cli_admin() {
        for service_type in [
            ServiceType::ComputerAgentRunner,
            ServiceType::WebAgentRunner,
        ] {
            let ports = agent_service_ports(&service_type);
            assert!(
                !ports.iter().any(|p| p.port == APP_CLI_ADMIN_PORT as i32),
                "{service_type} 容器内不跑 app-cli，不应暴露 3010"
            );
            // 基础六端口恒在
            assert_eq!(ports.len(), 6, "{service_type} 端口数应为基础六端口");
        }
    }

    #[test]
    fn service_exposes_port_matches_declared_only() {
        let svc = agent_service_object(
            "ns",
            "rcoder-app-builder-1-svc",
            "1",
            &ServiceType::UserappBuilder,
        );
        assert!(service_exposes_port(&svc, APP_CLI_ADMIN_PORT));
        assert!(!service_exposes_port(&svc, 9999));
        let legacy = Service {
            metadata: ObjectMeta::default(),
            spec: Some(ServiceSpec {
                ports: Some(agent_service_ports(&ServiceType::ComputerAgentRunner)),
                ..Default::default()
            }),
            status: None,
        };
        assert!(!service_exposes_port(&legacy, APP_CLI_ADMIN_PORT));
    }
}
