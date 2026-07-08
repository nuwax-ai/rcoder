//! K8s Deployment 生命周期管理（UserApp 专用）
//!
//! 与 agent 的裸 Pod 模式（k8s_pod.rs）区分：UserApp 走 Deployment，
//! 支持 scale（start/stop）与 rollout restart。
//!
//! 资源 label `app.kubernetes.io/managed-by=rcoder-app-manager`，与 agent 的
//! `rcoder-runtime` 物理隔离——cleanup_task 基于 `projects` 内存表扫描，UserApp
//! 不进该表；label 差异作为第二道防线（供对账接口 list，及防御未来按 label 的扫描）。
//!
//! 存储复用 rcoder-workspace RWX PVC + subPath `workspace/apps/{app_id}`（rcoder Pod 与 app
//! Pod 共享，app_manager 文件管理直接读写）。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    AppPortSpec, AppPortStatus, ContainerCreateParams, ContainerLogEntry, ContainerRuntimeError,
    ContainerRuntimeResult, DeploymentStatus, ExposeType,
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
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
#[cfg(feature = "kubernetes")]
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use tracing::{info, warn};

#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;

#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::KubernetesRuntime;

#[cfg(feature = "kubernetes")]
const APP_MANAGED_BY: &str = "rcoder-app-manager";
#[cfg(feature = "kubernetes")]
const APP_LABEL_PREFIX: &str = "app.kubernetes.io";
#[cfg(feature = "kubernetes")]
const RCODER_LABEL_PREFIX: &str = "rcoder.io";
/// UserApp Pod 主容器名：build_app_deployment 创建、deployment_to_status 按此名定位状态
#[cfg(feature = "kubernetes")]
const APP_CONTAINER_NAME: &str = "app";

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

    fn app_config_name(&self, app_id: &str) -> String {
        format!("{}-config", self.app_deployment_name(app_id))
    }

    fn app_secret_name(&self, app_id: &str) -> String {
        format!("{}-secret", self.app_deployment_name(app_id))
    }

    /// app ClusterIP Service 名（供 HTTPRoute backendRef / 集群内访问）
    pub fn app_service_name(&self, app_id: &str) -> String {
        format!("{}-svc", self.app_deployment_name(app_id))
    }

    fn app_http_route_name(&self, app_id: &str) -> String {
        format!("{}-route", self.app_deployment_name(app_id))
    }

    fn app_nodeport_name(&self, app_id: &str) -> String {
        format!("{}-nodeport", self.app_deployment_name(app_id))
    }

    /// app workspace PVC 名（复用 rcoder-workspace RWX PVC）
    /// 从 env `RCODER_WORKSPACE_PVC_NAME` 读，兜底 `{namespace}-rcoder-workspace`
    fn app_workspace_pvc_name(&self) -> String {
        std::env::var("RCODER_WORKSPACE_PVC_NAME")
            .unwrap_or_else(|_| format!("{}-rcoder-workspace", self.namespace))
    }

    /// 构建 app 专用 label（与 agent 物理隔离）。
    ///
    /// `tenant_id`/`space_id` 作为可选标签加入（供 label 过滤/对账）。
    /// **注意**：Deployment/Service 的 `.spec.selector` 必须用稳定 core（不含 tenant/space），
    /// 因为 selector 创建后不可变；tenant/space 若变更会导致 SSA apply 冲突。故 selector 一律
    /// 调 `build_app_labels(app_id, None, None)` 取 core，metadata/template 才用 full。
    fn build_app_labels(
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
    fn ssa_patch_params() -> PatchParams {
        PatchParams {
            field_manager: Some(APP_MANAGED_BY.to_string()),
            force: true,
            ..Default::default()
        }
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

    /// apply ConfigMap（存 env，非敏感）—— SSA create-or-update
    async fn apply_app_configmap(
        &self,
        app_id: &str,
        env: &std::collections::HashMap<String, String>,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(self.app_config_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id, tenant_id, space_id)),
                ..Default::default()
            },
            data: Some(env.clone().into_iter().collect()),
            ..Default::default()
        };
        let body = serde_json::to_value(&cm)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize configmap: {e}")))?;
        self.configmaps_api()
            .patch(
                &self.app_config_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply configmap: {e}")))?;
        Ok(())
    }

    /// apply Secret（存 secrets，敏感）—— SSA create-or-update
    async fn apply_app_secret(
        &self,
        app_id: &str,
        secrets: &std::collections::HashMap<String, String>,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        // K8s Secret data 需要 base64；StringData 更方便
        use k8s_openapi::api::core::v1::Secret;
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(self.app_secret_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id, tenant_id, space_id)),
                ..Default::default()
            },
            string_data: Some(secrets.clone().into_iter().collect()),
            ..Default::default()
        };
        let body = serde_json::to_value(&secret)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize secret: {e}")))?;
        self.secrets_api()
            .patch(
                &self.app_secret_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply secret: {e}")))?;
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

        let tenant_id = params.tenant_id.as_deref();
        let space_id = params.space_id.as_deref();
        // selector 用稳定 core（创建后不可变），metadata/template 用 full（含 tenant/space）
        let selector_labels = self.build_app_labels(app_id, None, None);
        let full_labels = self.build_app_labels(app_id, tenant_id, space_id);

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
            // ephemeral-storage：限制 overlay 可写层。优先 ephemeral_storage，回退 storage。
            // 修复此前 storage 字段被完全忽略的问题：UserApp 复用共享 PVC subPath 无独立配额,
            // storage 现用于限制 overlay 可写层（与 ephemeral_storage 同义）。
            let es = req
                .ephemeral_storage
                .clone()
                .or_else(|| req.storage.clone());
            if let Some(es_val) = es {
                limits.insert("ephemeral-storage".to_string(), Quantity(es_val));
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
        // ConfigMap/Secret 均设 optional=true：只有 env/secrets 非空时才建，引用安全。
        let env_from = Some(vec![
            EnvFromSource {
                config_map_ref: Some(ConfigMapEnvSource {
                    name: self.app_config_name(app_id),
                    optional: Some(true),
                }),
                ..Default::default()
            },
            EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: self.app_secret_name(app_id),
                    optional: Some(true),
                }),
                ..Default::default()
            },
        ]);

        // 额外直接注入 APP_ID 环境变量
        let env = Some(vec![EnvVar {
            name: "APP_ID".to_string(),
            value: Some(app_id.to_string()),
            ..Default::default()
        }]);

        // workspace PVC 挂载（复用 rcoder-workspace RWX，subPath workspace/apps/{app_id} → /app）
        // subPath 带 workspace 前缀以复用 rcoder Pod 已挂的 `workspace` subPath：
        // rcoder /app/project_workspace = PVC `/workspace`，故 rcoder /app/project_workspace/apps/{app_id}
        // = PVC `/workspace/apps/{app_id}` = app Pod /app，共享读写。
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
            sub_path: Some(format!("workspace/apps/{app_id}")),
            read_only: Some(false),
            ..Default::default()
        }]);

        let container = K8sContainer {
            name: APP_CONTAINER_NAME.to_string(),
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
                labels: Some(full_labels.clone()),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                selector: LabelSelector {
                    match_labels: Some(selector_labels),
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(full_labels),
                        // env/secrets 改的是 ConfigMap/Secret 数据，env_from 引用名不变 →
                        // 不触发 rollout。此 annotation 让"内容变 → hash 变 → spec 变 → 自动
                        // rollout"，使 env 更新对运行中 Pod 生效（K8s 标准模式）。
                        annotations: Some(config_hash_annotations(params)),
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

    /// apply ClusterIP Service（暴露 app 端口）—— SSA create-or-update
    async fn apply_app_service(
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
        let tenant_id = params.tenant_id.as_deref();
        let space_id = params.space_id.as_deref();
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(self.app_service_name(app_id)),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.build_app_labels(app_id, tenant_id, space_id)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                // selector 用稳定 core（创建后不可变）
                selector: Some(self.build_app_labels(app_id, None, None)),
                ports: Some(ports),
                ..Default::default()
            }),
            ..Default::default()
        };
        let body = serde_json::to_value(&svc)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize service: {e}")))?;
        self.services_api()
            .patch(
                &self.app_service_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply app service: {e}")))?;
        Ok(())
    }

    /// apply HTTPRoute（HTTP 端口 → Gateway）—— SSA create-or-update，path prefix `/apps/{app_id}`
    async fn apply_app_httproute(
        &self,
        app_id: &str,
        gateway_name: &str,
        gateway_namespace: &str,
        http_port: u16,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
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
                "labels": self.build_app_labels(app_id, tenant_id, space_id),
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
            .patch(
                &self.app_http_route_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(route),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply httproute: {e}")))?;
        Ok(())
    }

    /// apply NodePort Service（TCP 端口对外暴露）—— SSA create-or-update，
    /// 返回实际分配的 node_port 列表（server 分配，apply 后从返回对象读取）
    async fn apply_app_nodeport(
        &self,
        app_id: &str,
        tcp_ports: &[AppPortSpec],
        tenant_id: Option<&str>,
        space_id: Option<&str>,
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
                labels: Some(self.build_app_labels(app_id, tenant_id, space_id)),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("NodePort".to_string()),
                // selector 用稳定 core（创建后不可变）
                selector: Some(self.build_app_labels(app_id, None, None)),
                ports: Some(service_ports),
                ..Default::default()
            }),
            ..Default::default()
        };
        let body = serde_json::to_value(&svc)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize nodeport: {e}")))?;
        let created = self
            .services_api()
            .patch(
                &self.app_nodeport_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply nodeport: {e}")))?;

        // 提取实际分配的 node_port：按 name（回退 port 号）关联，避免 server 返回顺序与
        // 请求顺序不一致时 name 与 external_port 配错。
        use std::collections::HashMap;
        let mut result = vec![];
        if let Some(spec) = created.spec
            && let Some(ports) = spec.ports
        {
            let np_by_key: HashMap<String, u16> = ports
                .iter()
                .filter_map(|p| {
                    let np = p.node_port? as u16;
                    let key = p.name.clone().unwrap_or_else(|| format!("{}", p.port));
                    Some((key, np))
                })
                .collect();
            for req in tcp_ports {
                if let Some(&np) = np_by_key.get(&req.name) {
                    result.push(AppPortStatus {
                        name: req.name.clone(),
                        port: req.port,
                        expose_type: ExposeType::Tcp,
                        external_port: Some(np),
                    });
                }
            }
        }
        Ok(result)
    }

    /// apply Deployment（SSA create-or-update）。抽出供 create_app_resources 与
    /// patch_deployment（Phase 3）复用。
    async fn apply_app_deployment(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<()> {
        let deployment = self.build_app_deployment(app_id, params)?;
        let body = serde_json::to_value(&deployment)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize deployment: {e}")))?;
        self.deployments_api()
            .patch(
                &self.app_deployment_name(app_id),
                &Self::ssa_patch_params(),
                &Patch::Apply(body),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("apply deployment: {e}")))?;
        Ok(())
    }

    /// 创建 UserApp 的全部 K8s 资源（SSA apply，幂等 create-or-update）：
    /// ConfigMap/Secret/Service/Deployment/HTTPRoute/NodePort。
    pub async fn create_app_resources(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
        gateway_name: Option<&str>,
        gateway_namespace: Option<&str>,
    ) -> ContainerRuntimeResult<Vec<AppPortStatus>> {
        let tenant_id = params.tenant_id.as_deref();
        let space_id = params.space_id.as_deref();
        // 1. ConfigMap（env）
        if let Some(env) = &params.env
            && !env.is_empty()
        {
            self.apply_app_configmap(app_id, env, tenant_id, space_id)
                .await?;
        }
        // 2. Secret（secrets）
        if let Some(secrets) = &params.secrets
            && !secrets.is_empty()
        {
            self.apply_app_secret(app_id, secrets, tenant_id, space_id)
                .await?;
        }
        // 3. Service（ClusterIP，所有端口；HTTPRoute 用它做 backendRef）
        self.apply_app_service(app_id, params).await?;
        // 4. Deployment（SSA apply）
        self.apply_app_deployment(app_id, params).await?;
        info!("[K8S-APP] Deployment applied for app: {app_id}");
        // 5. HTTPRoute（HTTP 端口）—— 失败降级：app 主体（Deployment）已创建不可回滚，
        // HTTPRoute 属暴露层（依赖 Gateway/CRD），失败时 warn + 跳过，不阻塞 app 创建。
        // 避免"Deployment 已建 + create_app 整体失败"导致调用方重试时 name 冲突。
        let mut external_ports: Vec<AppPortStatus> = vec![];
        if let (Some(ports), Some(gw), Some(gw_ns)) =
            (params.ports.as_ref(), gateway_name, gateway_namespace)
            && let Some(http_port) = ports.iter().find(|p| p.expose_type == ExposeType::Http)
        {
            match self
                .apply_app_httproute(app_id, gw, gw_ns, http_port.port, tenant_id, space_id)
                .await
            {
                Ok(_) => {
                    external_ports.push(AppPortStatus {
                        name: http_port.name.clone(),
                        port: http_port.port,
                        expose_type: ExposeType::Http,
                        external_port: None,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        "[K8S-APP] HTTPRoute apply 失败，app 主体已创建但 HTTP 入口暂不可用（待 Gateway/CRD 就绪后 reconcile）: {}",
                        e
                    );
                }
            }
        }
        // 6. NodePort（TCP 端口）
        if let Some(ports) = params.ports.as_ref() {
            let tcp_ports: Vec<AppPortSpec> = ports
                .iter()
                .filter(|p| p.expose_type == ExposeType::Tcp)
                .cloned()
                .collect();
            let nodeports = self
                .apply_app_nodeport(app_id, &tcp_ports, tenant_id, space_id)
                .await?;
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

    /// 查询单个 app 的运行时状态（实时查 Deployment + Pod）
    pub async fn get_app_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let name = self.app_deployment_name(app_id);
        let deploy = match self.deployments_api().get(&name).await {
            Ok(d) => d,
            Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(None),
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get deployment: {e}"
                )));
            }
        };
        Ok(Some(self.deployment_to_status(app_id, &deploy).await))
    }

    /// 列出所有 rcoder-app-manager 托管的 app 状态（对账用）
    pub async fn list_app_status(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        let lp = ListParams::default()
            .labels(&format!("{}/managed-by={APP_MANAGED_BY}", APP_LABEL_PREFIX));
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

    /// 拉取 app Pod 的 stdout/stderr 日志（最近 `tail` 行）。
    /// 按 `rcoder.io/app-id` label 定位 Pod；`timestamps=true` 时 K8s 在每行前缀 RFC3339。
    /// K8s logs API 合并 stdout/stderr 返回，stream 统一记 "stdout"。
    pub async fn app_logs(
        &self,
        app_id: &str,
        tail: u32,
        timestamps: bool,
    ) -> ContainerRuntimeResult<Vec<ContainerLogEntry>> {
        use kube::api::LogParams;
        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
        let pods = self
            .pods_api()
            .list(&lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("list pods for logs: {e}")))?;
        // 无 Pod（app stopped / 副本缩为 0）→ 返回空，与 Docker 侧"容器不存在→空日志"一致，
        // 避免 stopped app 查日志被误报 404（应用还在，只是当前无运行实例）。
        let Some(pod_name) = pods
            .items
            .into_iter()
            .next()
            .and_then(|p| p.metadata.name.clone())
        else {
            return Ok(vec![]);
        };
        let log_lp = LogParams {
            tail_lines: Some(tail as i64),
            timestamps,
            ..Default::default()
        };
        let raw = self
            .pods_api()
            .logs(&pod_name, &log_lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("pod logs: {e}")))?;
        // K8s logs API 合并 stdout/stderr，stream 统一记 "stdout"
        Ok(raw
            .lines()
            .map(|l| {
                let (ts, msg) = container_runtime_api::split_log_timestamp(l, timestamps);
                ContainerLogEntry {
                    timestamp: ts,
                    stream: "stdout".to_string(),
                    message: msg,
                }
            })
            .collect())
    }

    /// 启动日志流（follow）：返回 mpsc::Receiver。内部 spawn 任务读 K8s `log_stream(follow)`，
    /// 逐行 send 到 channel。receiver drop（客户端断开）→ send 出错 → 任务退出释放日志源。
    ///
    /// 命名 `_inner` 与同文件 `app_logs`/`scale_app`/`restart_app` 约定一致（trait 同名方法
    /// 转调不同名的 inherent，避免 trait impl 内 self.同名() 依赖方法解析优先级）。
    pub async fn stream_app_logs_inner(
        &self,
        app_id: &str,
        tail: u32,
    ) -> ContainerRuntimeResult<container_runtime_api::mpsc::Receiver<ContainerLogEntry>> {
        use futures_util::{AsyncBufReadExt, StreamExt};
        use kube::api::LogParams;

        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
        let pods = self.pods_api().list(&lp).await.map_err(|e| {
            ContainerRuntimeError::K8sError(format!("list pods for log stream: {e}"))
        })?;
        let pod_name = pods
            .items
            .into_iter()
            .next()
            .and_then(|p| p.metadata.name.clone())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(format!(
                    "app {app_id} 当前无运行 Pod（可能已 stopped）"
                ))
            })?;
        let timestamps = true;
        let log_lp = LogParams {
            tail_lines: if tail > 0 { Some(tail as i64) } else { None },
            follow: true,
            timestamps,
            ..Default::default()
        };
        let reader = self
            .pods_api()
            .log_stream(&pod_name, &log_lp)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("log_stream: {e}")))?;
        let (tx, rx) = container_runtime_api::mpsc::channel::<ContainerLogEntry>(64);
        tokio::spawn(async move {
            // kube log_stream 返回 futures_util::AsyncBufRead；lines() 返回 Stream<Item=io::Result<String>>。
            // Box::pin 保证 Unpin（lines 需 Self: Unpin）。
            let reader = Box::pin(reader);
            let mut lines = reader.lines();
            while let Some(result) = lines.next().await {
                match result {
                    Ok(line) => {
                        let (ts, msg) =
                            container_runtime_api::split_log_timestamp(&line, timestamps);
                        let entry = ContainerLogEntry {
                            timestamp: ts,
                            stream: "stdout".to_string(),
                            message: msg,
                        };
                        if tx.send(entry).await.is_err() {
                            break; // 客户端断开，receiver 已 drop
                        }
                    }
                    Err(e) => {
                        warn!("[K8S-APP] log_stream 读行失败 (终止流): {e}");
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }

    /// Deployment 对象 → DeploymentStatus（含关联 Pod 的实时信息）
    async fn deployment_to_status(&self, app_id: &str, deploy: &Deployment) -> DeploymentStatus {
        let spec = deploy.spec.as_ref();
        let status = deploy.status.as_ref();
        let replicas = spec.and_then(|s| s.replicas).unwrap_or(0);
        let ready_replicas = status.and_then(|s| s.ready_replicas).unwrap_or(0);

        // 关联 Pod 信息（取一个；app 当前为单副本）。先于 phase 计算：需要用容器状态
        // （CrashLoopBackOff / ImagePullBackOff / 非 0 退出）判定启动失败 → phase=Error。
        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
        let (pod_ip, node, restart_count, started_at, error_message) =
            match self.pods_api().list(&lp).await {
                Ok(pods) => pods
                    .items
                    .into_iter()
                    .next()
                    .and_then(|p| {
                        let st = p.status?;
                        // 按容器名取 "app" 容器状态（防御 sidecar 注入后 pop() 取错容器）
                        let cs = st
                            .container_statuses
                            .and_then(|v| v.into_iter().find(|c| c.name == APP_CONTAINER_NAME))?;
                        // started_at：从 container state.running 提取实际启动时间
                        let started_at = cs
                            .state
                            .as_ref()
                            .and_then(|s| s.running.as_ref())
                            .and_then(|r| r.started_at.as_ref())
                            .map(|t| t.0.to_string());
                        // 启动失败原因（CrashLoop / 镜像拉取失败 / 异常退出）；正常拉起的中间态
                        // （ContainerCreating）不在此列，不会被误判为 Error。
                        let error_message = container_error_message(&cs);
                        Some((
                            st.pod_ip.unwrap_or_default(),
                            p.spec.and_then(|s| s.node_name).unwrap_or_default(),
                            cs.restart_count as u32,
                            started_at,
                            error_message,
                        ))
                    })
                    .unwrap_or_default(),
                Err(_) => (String::new(), String::new(), 0, None, None),
            };

        // phase：replicas=0 → Stopped；容器启动失败 → Error（优先于 ready 判定，避免
        // CrashLoop 期间偶发 ready_replicas>0 被误报 Running）；就绪副本达标 → Running；否则 Starting。
        let phase = if replicas == 0 {
            "Stopped".to_string()
        } else if error_message.is_some() {
            "Error".to_string()
        } else if ready_replicas >= replicas && ready_replicas > 0 {
            "Running".to_string()
        } else {
            "Starting".to_string()
        };

        // TCP 端口的 node_port：查 NodePort Service，按 port name 关联（name 与 container port 一致）
        // 在 NodePort Service 中的端口 = Tcp（external_port = node_port）；不在的 = Http。
        let tcp_nodeports: std::collections::HashMap<String, u16> = self
            .services_api()
            .get(&self.app_nodeport_name(app_id))
            .await
            .ok()
            .and_then(|svc| svc.spec)
            .and_then(|s| s.ports)
            .map(|ports| {
                ports
                    .into_iter()
                    .filter_map(|p| {
                        let name = p.name.unwrap_or_default();
                        let np = p.node_port.map(|n| n as u16)?;
                        Some((name, np))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 端口状态：从 Deployment spec 的 container ports 推导，
        // expose_type / external_port 由 NodePort Service 还原（Http 端口经 Gateway，不 per-port 分配）
        let ports = spec
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|s| s.containers.first())
            .and_then(|c| c.ports.as_ref())
            .map(|ps| {
                ps.iter()
                    .map(|p| {
                        let name = p.name.clone().unwrap_or_default();
                        let (expose_type, external_port) =
                            if let Some(np) = tcp_nodeports.get(&name) {
                                (ExposeType::Tcp, Some(*np))
                            } else {
                                (ExposeType::Http, None)
                            };
                        AppPortStatus {
                            name,
                            port: p.container_port as u16,
                            expose_type,
                            external_port,
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        DeploymentStatus {
            app_id: app_id.to_string(),
            replicas,
            ready_replicas,
            phase,
            message: error_message,
            pod_ip: if pod_ip.is_empty() {
                None
            } else {
                Some(pod_ip)
            },
            node: if node.is_empty() { None } else { Some(node) },
            restart_count,
            started_at,
            ports,
        }
    }
}

/// 从容器状态提取"启动失败"原因（供 phase=Error 的 message）。
///
/// 命中条件（任一）：
/// - `state.waiting.reason` ∈ {CrashLoopBackOff, ImagePullBackOff, ErrImagePull,
///   CreateContainerConfigError, CreateContainerError, InvalidImageName, RunContainerError,
///   StartError}（`ContainerCreating` 是正常拉起中间态，不在此列，不会被误判）
/// - `state.terminated.exit_code != 0`（容器异常退出）
///
/// CrashLoop 时当前 `state=waiting`，真实退出码在 `last_state.terminated`，一并附带，
/// 便于定位"挂在哪一次退出、退出码多少"。
fn container_error_message(cs: &k8s_openapi::api::core::v1::ContainerStatus) -> Option<String> {
    let state = cs.state.as_ref()?;
    const BAD_WAITING: &[&str] = &[
        "CrashLoopBackOff",
        "ImagePullBackOff",
        "ErrImagePull",
        "CreateContainerConfigError",
        "CreateContainerError",
        "InvalidImageName",
        "RunContainerError",
        "StartError",
    ];
    if let Some(w) = state.waiting.as_ref()
        && let Some(reason) = w.reason.as_ref()
        && BAD_WAITING.contains(&reason.as_str())
    {
        let detail = w
            .message
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        let term = cs
            .last_state
            .as_ref()
            .and_then(|ls| ls.terminated.as_ref())
            .map(|t| {
                format!(
                    " (last exit={}, reason={})",
                    t.exit_code,
                    t.reason.as_deref().unwrap_or("")
                )
            })
            .unwrap_or_default();
        return Some(format!("{reason}{detail}{term}"));
    }
    if let Some(t) = state.terminated.as_ref()
        && t.exit_code != 0
    {
        let reason = t.reason.as_deref().unwrap_or("");
        let msg = t
            .message
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        return Some(format!(
            "terminated: exit code={exit}, reason={reason}{msg}",
            exit = t.exit_code
        ));
    }
    None
}

/// 计算 env+secrets 内容的 hash，注入 pod template annotation。
///
/// 作用：env/secrets 走 ConfigMap/Secret，改内容时 `env_from` 引用名不变 → Deployment spec
/// 不变 → 不触发 rollout → 新 env 到不了运行中 Pod。此 hash 进 pod template，内容变即
/// annotation 变 → spec 变 → 自动 rollout。DefaultHasher 跨进程确定（固定 key），故同
/// 内容多次 apply 的 hash 稳定，不会引发误 rollout。
#[cfg(feature = "kubernetes")]
fn config_hash_annotations(
    params: &container_runtime_api::ContainerCreateParams,
) -> BTreeMap<String, String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for map in [params.env.as_ref(), params.secrets.as_ref()]
        .into_iter()
        .flatten()
    {
        let mut items: Vec<_> = map.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in items {
            k.hash(&mut h);
            v.hash(&mut h);
        }
    }
    let mut ann = BTreeMap::new();
    ann.insert(
        "rcoder.io/config-hash".to_string(),
        format!("{:016x}", h.finish()),
    );
    ann
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
