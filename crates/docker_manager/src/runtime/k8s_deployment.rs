//! K8s Deployment 生命周期管理（UserApp 专用）
//!
//! 与 agent 的裸 Pod 模式（k8s_pod.rs）区分：UserApp 走 Deployment，
//! 支持 scale（start/stop）与 rollout restart。
//!
//! 资源 label `app.kubernetes.io/managed-by=rcoder-app-manager`，与 agent 的
//! `rcoder-runtime` 物理隔离——cleanup_task 基于 `projects` 内存表扫描，UserApp
//! 不进该表；label 差异作为第二道防线（供对账接口 list，及防御未来按 label 的扫描）。
//!
//! 存储复用 rcoder-workspace RWX PVC + subPath `apps/{app_id}`（rcoder Pod 与 app
//! Pod 共享，app_manager 文件管理直接读写）。

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    AppPortSpec, AppPortStatus, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    DeploymentStatus, ExposeType,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapEnvSource, Container as K8sContainer, ContainerPort, EnvFromSource, EnvVar,
    PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, Probe, ResourceRequirements,
    SecretEnvSource, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
#[cfg(feature = "kubernetes")]
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use tracing::{info, warn};

#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::KubernetesRuntime;

#[cfg(feature = "kubernetes")]
const APP_MANAGED_BY: &str = "rcoder-app-manager";
#[cfg(feature = "kubernetes")]
const APP_LABEL_PREFIX: &str = "app.kubernetes.io";
#[cfg(feature = "kubernetes")]
const RCODER_LABEL_PREFIX: &str = "rcoder.io";

/// UserApp K8s 资源命名 + 创建/伸缩/重启/删除/查询（pub(crate)，由 ContainerRuntime
/// trait 的 Deployment 方法转调，rcoder 通过 trait 调用）。
#[cfg(feature = "kubernetes")]
impl KubernetesRuntime {
    /// app_id → Deployment 名
    pub fn app_deployment_name(&self, app_id: &str) -> String {
        format!("rcoder-app-{app_id}")
    }

    fn app_config_name(&self, app_id: &str) -> String {
        format!("rcoder-app-{app_id}-config")
    }

    fn app_secret_name(&self, app_id: &str) -> String {
        format!("rcoder-app-{app_id}-secret")
    }

    /// app ClusterIP Service 名（供 HTTPRoute backendRef / 集群内访问）
    pub fn app_service_name(&self, app_id: &str) -> String {
        format!("rcoder-app-{app_id}-svc")
    }

    fn app_http_route_name(&self, app_id: &str) -> String {
        format!("rcoder-app-{app_id}-route")
    }

    fn app_nodeport_name(&self, app_id: &str) -> String {
        format!("rcoder-app-{app_id}-nodeport")
    }

    /// app workspace PVC 名（复用 rcoder-workspace RWX PVC）
    /// 从 env `RCODER_WORKSPACE_PVC_NAME` 读，兜底 `{namespace}-rcoder-workspace`
    fn app_workspace_pvc_name(&self) -> String {
        std::env::var("RCODER_WORKSPACE_PVC_NAME")
            .unwrap_or_else(|_| format!("{}-rcoder-workspace", self.namespace))
    }

    /// 构建 app 专用 label（与 agent 物理隔离）
    fn build_app_labels(&self, app_id: &str) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert(
            format!("{}/name", APP_LABEL_PREFIX),
            "user-app".to_string(),
        );
        labels.insert(format!("{}/instance", APP_LABEL_PREFIX), app_id.to_string());
        labels.insert(
            format!("{}/managed-by", APP_LABEL_PREFIX),
            APP_MANAGED_BY.to_string(),
        );
        labels.insert(format!("{}/part-of", APP_LABEL_PREFIX), "rcoder".to_string());
        labels.insert(format!("{}/app-id", RCODER_LABEL_PREFIX), app_id.to_string());
        labels
    }

    fn pods_api(&self) -> Api<k8s_openapi::api::core::v1::Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn deployments_api(&self) -> Api<Deployment> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn services_api(&self) -> Api<Service> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn configmaps_api(&self) -> Api<ConfigMap> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn secrets_api(&self) -> Api<k8s_openapi::api::core::v1::Secret> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// 创建 ConfigMap（存 env，非敏感）
    async fn create_app_configmap(
        &self,
        app_id: &str,
        env: &std::collections::HashMap<String, String>,
    ) -> ContainerRuntimeResult<()> {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(self.app_config_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id)),
                ..Default::default()
            },
            data: Some(env.clone().into_iter().collect()),
            ..Default::default()
        };
        self.configmaps_api()
            .create(&PostParams::default(), &cm)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create configmap: {e}")))?;
        Ok(())
    }

    /// 创建 Secret（存 secrets，敏感）
    async fn create_app_secret(
        &self,
        app_id: &str,
        secrets: &std::collections::HashMap<String, String>,
    ) -> ContainerRuntimeResult<()> {
        // K8s Secret data 需要 base64；StringData 更方便
        use k8s_openapi::api::core::v1::Secret;
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(self.app_secret_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id)),
                ..Default::default()
            },
            string_data: Some(secrets.clone().into_iter().collect()),
            ..Default::default()
        };
        self.secrets_api()
            .create(&PostParams::default(), &secret)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create secret: {e}")))?;
        Ok(())
    }

    /// 构建 Deployment 资源
    fn build_app_deployment(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<Deployment> {
        let image = params.image_override.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "UserApp create_deployment requires image_override".to_string(),
            )
        })?;

        let labels = self.build_app_labels(app_id);

        // 端口
        let ports: Vec<ContainerPort> = params
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| ContainerPort {
                        name: Some(p.name.clone()),
                        container_port: p.port as i32,
                        ..Default::default()
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 资源
        let resources = params.app_resources.as_ref().and_then(|req| {
            let mut limits = BTreeMap::new();
            if let Some(cpu) = &req.cpu {
                limits.insert("cpu".to_string(), Quantity(cpu.clone()));
            }
            if let Some(mem) = &req.memory {
                limits.insert("memory".to_string(), Quantity(mem.clone()));
            }
            if limits.is_empty() {
                None
            } else {
                Some(ResourceRequirements {
                    limits: Some(limits),
                    ..Default::default()
                })
            }
        });

        // 健康检查 probe
        let (liveness, readiness) = params.health_check.as_ref().map_or((None, None), |hc| {
            let probe = build_probe(hc);
            (probe.clone(), probe)
        });

        // 环境变量（ConfigMap + Secret 通过 envFrom 引用）
        let env_from = {
            let mut refs = Vec::new();
            // ConfigMap 只有在 env 非空时才建；这里总是引用（create 时已建则引用安全）
            refs.push(EnvFromSource {
                config_map_ref: Some(ConfigMapEnvSource {
                    name: self.app_config_name(app_id),
                    optional: Some(true),
                }),
                ..Default::default()
            });
            refs.push(EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: self.app_secret_name(app_id),
                    optional: Some(true),
                }),
                ..Default::default()
            });
            Some(refs)
        };

        // 额外直接注入 APP_ID 环境变量
        let env = Some(vec![EnvVar {
            name: "APP_ID".to_string(),
            value: Some(app_id.to_string()),
            ..Default::default()
        }]);

        // workspace PVC 挂载（复用 rcoder-workspace RWX，subPath apps/{app_id} → /app）
        let volumes = Some(vec![Volume {
            name: "app-workspace".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: self.app_workspace_pvc_name(),
                read_only: Some(false),
            }),
            ..Default::default()
        }]);
        let volume_mounts = Some(vec![VolumeMount {
            name: "app-workspace".to_string(),
            mount_path: "/app".to_string(),
            sub_path: Some(format!("apps/{app_id}")),
            read_only: Some(false),
            ..Default::default()
        }]);

        let container = K8sContainer {
            name: "app".to_string(),
            image: Some(image),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: params.command.clone(),
            args: params.args.clone(),
            env,
            env_from,
            ports: if ports.is_empty() { None } else { Some(ports) },
            resources,
            volume_mounts,
            liveness_probe: liveness,
            readiness_probe: readiness,
            ..Default::default()
        };

        let pod_spec = PodSpec {
            volumes,
            containers: vec![container],
            restart_policy: Some("Always".to_string()),
            ..Default::default()
        };

        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some(self.app_deployment_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels),
                        ..Default::default()
                    }),
                    spec: Some(pod_spec),
                },
                ..Default::default()
            }),
            status: None,
        };
        Ok(deployment)
    }

    /// 创建 ClusterIP Service（暴露 app 端口，供 HTTPRoute backendRef / 集群内访问）
    async fn create_app_service(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<()> {
        let ports: Vec<ServicePort> = params
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| ServicePort {
                        name: Some(p.name.clone()),
                        port: p.port as i32,
                        target_port: Some(IntOrString::Int(p.port as i32)),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        if ports.is_empty() {
            return Ok(());
        }
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(self.app_service_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                selector: Some(self.build_app_labels(app_id)),
                ports: Some(ports),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.services_api()
            .create(&PostParams::default(), &svc)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create app service: {e}")))?;
        Ok(())
    }

    /// 创建 HTTPRoute（HTTP 端口 → Gateway），path prefix `/apps/{app_id}`
    async fn create_app_httproute(
        &self,
        app_id: &str,
        gateway_name: &str,
        gateway_namespace: &str,
        http_port: u16,
    ) -> ContainerRuntimeResult<()> {
        let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
        let api_resource = ApiResource::from_gvk(&gvk);
        let routes: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.namespace, &api_resource);

        let route = serde_json::json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": {
                "name": self.app_http_route_name(app_id),
                "namespace": self.namespace,
                "labels": self.build_app_labels(app_id),
            },
            "spec": {
                "parentRefs": [{
                    "name": gateway_name,
                    "namespace": gateway_namespace,
                }],
                "rules": [{
                    "matches": [{
                        "path": {
                            "type": "PathPrefix",
                            "value": format!("/apps/{app_id}")
                        }
                    }],
                    "backendRefs": [{
                        "name": self.app_service_name(app_id),
                        "port": http_port as i32,
                    }]
                }]
            }
        });

        routes
            .create(
                &PostParams::default(),
                &serde_json::from_value(route)
                    .map_err(|e| ContainerRuntimeError::K8sError(format!("parse httproute: {e}")))?,
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create httproute: {e}")))?;
        Ok(())
    }

    /// 创建 NodePort Service（TCP 端口对外暴露），返回实际分配的 node_port 列表
    async fn create_app_nodeport(
        &self,
        app_id: &str,
        tcp_ports: &[AppPortSpec],
    ) -> ContainerRuntimeResult<Vec<AppPortStatus>> {
        if tcp_ports.is_empty() {
            return Ok(vec![]);
        }
        let service_ports: Vec<ServicePort> = tcp_ports
            .iter()
            .map(|p| ServicePort {
                name: Some(p.name.clone()),
                port: p.port as i32,
                target_port: Some(IntOrString::Int(p.port as i32)),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            })
            .collect();
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(self.app_nodeport_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("NodePort".to_string()),
                selector: Some(self.build_app_labels(app_id)),
                ports: Some(service_ports),
                ..Default::default()
            }),
            ..Default::default()
        };
        let created = self
            .services_api()
            .create(&PostParams::default(), &svc)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create nodeport: {e}")))?;

        // 提取实际分配的 node_port
        let mut result = vec![];
        if let Some(spec) = created.spec
            && let Some(ports) = spec.ports
        {
            for (i, p) in ports.iter().enumerate() {
                if let Some(np) = p.node_port {
                    result.push(AppPortStatus {
                        name: tcp_ports
                            .get(i)
                            .map(|p| p.name.clone())
                            .unwrap_or_default(),
                        port: p.port as u16,
                        expose_type: ExposeType::Tcp,
                        external_port: Some(np as u16),
                    });
                }
            }
        }
        Ok(result)
    }

    /// 创建 UserApp 的全部 K8s 资源：ConfigMap/Secret/Deployment/Service/HTTPRoute/NodePort
    pub async fn create_app_resources(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
        gateway_name: Option<&str>,
        gateway_namespace: Option<&str>,
    ) -> ContainerRuntimeResult<Vec<AppPortStatus>> {
        // 1. ConfigMap（env）
        if let Some(env) = &params.env
            && !env.is_empty()
        {
            self.create_app_configmap(app_id, env).await?;
        }
        // 2. Secret（secrets）
        if let Some(secrets) = &params.secrets
            && !secrets.is_empty()
        {
            self.create_app_secret(app_id, secrets).await?;
        }
        // 3. Service（ClusterIP，所有端口；HTTPRoute 用它做 backendRef）
        self.create_app_service(app_id, params).await?;
        // 4. Deployment
        let deployment = self.build_app_deployment(app_id, params)?;
        self.deployments_api()
            .create(&PostParams::default(), &deployment)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create deployment: {e}")))?;
        info!("[K8S-APP] Deployment created for app: {app_id}");
        // 5. HTTPRoute（HTTP 端口）
        let mut external_ports: Vec<AppPortStatus> = vec![];
        if let (Some(ports), Some(gw), Some(gw_ns)) =
            (params.ports.as_ref(), gateway_name, gateway_namespace)
        {
            if let Some(http_port) = ports.iter().find(|p| p.expose_type == ExposeType::Http) {
                self.create_app_httproute(app_id, gw, gw_ns, http_port.port).await?;
                external_ports.push(AppPortStatus {
                    name: http_port.name.clone(),
                    port: http_port.port,
                    expose_type: ExposeType::Http,
                    external_port: None,
                });
            }
        }
        // 6. NodePort（TCP 端口）
        if let Some(ports) = params.ports.as_ref() {
            let tcp_ports: Vec<AppPortSpec> = ports
                .iter()
                .filter(|p| p.expose_type == ExposeType::Tcp)
                .cloned()
                .collect();
            let nodeports = self.create_app_nodeport(app_id, &tcp_ports).await?;
            external_ports.extend(nodeports);
        }
        Ok(external_ports)
    }

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
    pub async fn delete_app_resources(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let dp = DeleteParams::default();
        // 404 视为已删除，不报错
        let _ = self.deployments_api().delete(&self.app_deployment_name(app_id), &dp).await;
        let _ = self.services_api().delete(&self.app_service_name(app_id), &dp).await;
        let _ = self.services_api().delete(&self.app_nodeport_name(app_id), &dp).await;
        let _ = self.configmaps_api().delete(&self.app_config_name(app_id), &dp).await;
        let _ = self.secrets_api().delete(&self.app_secret_name(app_id), &dp).await;
        // HTTPRoute（动态资源）
        let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
        let api_resource = ApiResource::from_gvk(&gvk);
        let routes: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.namespace, &api_resource);
        let _ = routes.delete(&self.app_http_route_name(app_id), &dp).await;
        info!("[K8S-APP] K8s resources deleted for app: {app_id}");
        Ok(())
    }

    /// 查询单个 app 的运行时状态（实时查 Deployment + Pod）
    pub async fn get_app_status(&self, app_id: &str) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let name = self.app_deployment_name(app_id);
        let deploy = match self.deployments_api().get(&name).await {
            Ok(d) => d,
            Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(None),
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get deployment: {e}"
                )))
            }
        };
        Ok(Some(self.deployment_to_status(app_id, &deploy).await))
    }

    /// 列出所有 rcoder-app-manager 托管的 app 状态（对账用）
    pub async fn list_app_status(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        let lp = ListParams::default().labels(&format!(
            "{}/managed-by={APP_MANAGED_BY}",
            APP_LABEL_PREFIX
        ));
        let deploys = self
            .deployments_api()
            .list(&lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("list deployments: {e}")))?;
        let mut out = vec![];
        for d in deploys {
            // app_id 从 label rcoder.io/app-id 取
            let app_id = d
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(&format!("{}/app-id", RCODER_LABEL_PREFIX)))
                .cloned()
                .unwrap_or_default();
            if app_id.is_empty() {
                continue;
            }
            out.push(self.deployment_to_status(&app_id, &d).await);
        }
        Ok(out)
    }

    /// Deployment 对象 → DeploymentStatus（含关联 Pod 的实时信息）
    async fn deployment_to_status(&self, app_id: &str, deploy: &Deployment) -> DeploymentStatus {
        let spec = deploy.spec.as_ref();
        let status = deploy.status.as_ref();
        let replicas = spec.and_then(|s| s.replicas).unwrap_or(0);
        let ready_replicas = status.and_then(|s| s.ready_replicas).unwrap_or(0);

        let phase = if replicas == 0 {
            "Stopped".to_string()
        } else if ready_replicas >= replicas && ready_replicas > 0 {
            "Running".to_string()
        } else {
            "Starting".to_string()
        };

        // 关联 Pod 信息（取一个）
        let lp = ListParams::default().labels(&format!(
            "{}/app-id={app_id}",
            RCODER_LABEL_PREFIX
        ));
        let (pod_ip, node, restart_count, started_at) = match self.pods_api().list(&lp).await {
            Ok(pods) => pods
                .items
                .into_iter()
                .next()
                .and_then(|p| {
                    let st = p.status?;
                    let cs = st.container_statuses.and_then(|mut v| v.pop())?;
                    Some((
                        st.pod_ip.unwrap_or_default(),
                        p.spec.and_then(|s| s.node_name).unwrap_or_default(),
                        cs.restart_count as u32,
                        cs.last_state.as_ref().map(|_| String::new()),
                    ))
                })
                .unwrap_or_default(),
            Err(_) => (String::new(), String::new(), 0, None),
        };

        // 端口状态（从 Deployment spec 的 container ports 推导；external_port 需查 Service/NodePort）
        let ports = spec
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|s| s.containers.first())
            .and_then(|c| c.ports.as_ref())
            .map(|ps| {
                ps.iter()
                    .map(|p| AppPortStatus {
                        name: p.name.clone().unwrap_or_default(),
                        port: p.container_port as u16,
                        expose_type: ExposeType::Http, // 精确类型需查 Service，此处兜底
                        external_port: None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let _ = started_at; // TODO: 从 Pod status.startTime 提取
        DeploymentStatus {
            app_id: app_id.to_string(),
            replicas,
            ready_replicas,
            phase,
            pod_ip: if pod_ip.is_empty() { None } else { Some(pod_ip) },
            node: if node.is_empty() { None } else { Some(node) },
            restart_count,
            started_at: None,
            ports,
        }
    }
}

/// 健康检查配置 → K8s Probe
#[cfg(feature = "kubernetes")]
fn build_probe(hc: &container_runtime_api::AppHealthCheck) -> Option<Probe> {
    use container_runtime_api::HealthCheckType;
    let init = hc.initial_delay_seconds.map(|s| s as i32);
    let period = hc.period_seconds.map(|s| s as i32);
    match hc.check_type {
        HealthCheckType::None | HealthCheckType::Exec => None,
        HealthCheckType::Http => {
            let port = hc.port.unwrap_or(80);
            Some(Probe {
                http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                    path: Some(hc.path.clone().unwrap_or_else(|| "/".to_string())),
                    port: IntOrString::Int(port as i32),
                    ..Default::default()
                }),
                initial_delay_seconds: init,
                period_seconds: period,
                ..Default::default()
            })
        }
        HealthCheckType::Tcp => {
            let port = hc.port.unwrap_or(80);
            Some(Probe {
                tcp_socket: Some(k8s_openapi::api::core::v1::TCPSocketAction {
                    port: IntOrString::Int(port as i32),
                    ..Default::default()
                }),
                initial_delay_seconds: init,
                period_seconds: period,
                ..Default::default()
            })
        }
    }
}

// 当未启用 kubernetes feature 时，提供空实现占位（避免 mod.rs 报错）
#[cfg(not(feature = "kubernetes"))]
#[allow(dead_code)]
struct K8sDeploymentOpsStub;
